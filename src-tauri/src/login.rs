//! Opening the app at login.
//!
//! The app registers itself with the system's login item service
//! (`SMAppService.mainApp`, macOS 13 and later). It then appears in System
//! Settings › General › Login Items › Open at Login under its own name and
//! icon, and the entry there and the menu item are one setting: the app asks
//! the system every time and keeps no copy of the answer.
//!
//! The app turns it on by itself once, the first time it runs from the
//! Applications folder. A small file records that this happened (or that the
//! user chose from the menu), so a user who turns it off, here or in System
//! Settings, is never overridden.
//!
//! A launch at login only shows the menu bar icon. It is recognised by the
//! open-application Apple event, which the system marks when it opens a login
//! item; a launch it cannot recognise is treated as the user opening the app.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Set in a development run to make that launch count as one at login.
pub const SIMULATE_LAUNCH_ENV: &str = "SINTBOT_DESKTOP_LAUNCHED_AT_LOGIN";
/// Set to `1` in a development run to try the menu item against a file that
/// stands in for System Settings, instead of having no menu item at all.
pub const SIMULATE_ENV: &str = "SINTBOT_DESKTOP_LOGIN_ITEM_SIMULATED";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    On,
    /// Not registered, or removed by the user in System Settings.
    Off,
    /// Registered, but waiting for the user to allow it in System Settings.
    NeedsApproval,
}

impl State {
    pub fn checked(self) -> bool { self == State::On }
    pub fn name(self) -> &'static str {
        match self { State::On => "on", State::Off => "off", State::NeedsApproval => "needs-approval" }
    }
    fn parse(text: &str) -> State {
        match text.trim() { "on" => State::On, "needs-approval" => State::NeedsApproval, _ => State::Off }
    }
}

/// The system's login item service for this app.
pub trait System: Send + Sync {
    fn state(&self) -> State;
    fn enable(&self) -> Result<(), String>;
    fn disable(&self) -> Result<(), String>;
    /// System Settings › General › Login Items, where the user allows or removes it.
    fn open_settings(&self);
}

/// Remembers that the one-time default was applied, or that the user chose.
/// Only its presence matters; the content is for people reading it.
pub struct Record { path: PathBuf }

impl Record {
    pub fn new(path: PathBuf) -> Self { Record { path } }
    pub fn exists(&self) -> bool { self.path.is_file() }
    pub fn write(&self, how: &str) -> Result<(), String> {
        let dir = self.path.parent().ok_or("the login item record has no directory")?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let at = time::OffsetDateTime::now_utc().replace_nanosecond(0).ok()
            .and_then(|now| now.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default();
        let body = serde_json::json!({ "openAtLogin": "decided", "by": how, "at": at });
        let tmp = self.path.with_extension(format!("json.tmp-{}", std::process::id()));
        fs::write(&tmp, format!("{body}\n")).map_err(|e| format!("{}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path).map_err(|e| format!("{}: {e}", self.path.display()))
    }
    pub fn clear(&self) { let _ = fs::remove_file(&self.path); }
}

pub struct LoginItem {
    system: Box<dyn System>,
    record: Record,
    /// Whether this copy may turn the setting on by itself (it runs from an
    /// Applications folder). The menu item works either way.
    may_default: bool,
    changing: Mutex<()>,
    reading: AtomicBool,
}

impl LoginItem {
    pub fn new(system: Box<dyn System>, record: Record, may_default: bool) -> Self {
        LoginItem { system, record, may_default, changing: Mutex::new(()), reading: AtomicBool::new(false) }
    }

    pub fn state(&self) -> State { self.system.state() }

    /// `None` while another read is under way, so hovering over the menu bar
    /// icon does not queue up requests to the system.
    pub fn try_state(&self) -> Option<State> {
        if self.reading.swap(true, Ordering::SeqCst) { return None }
        let state = self.system.state();
        self.reading.store(false, Ordering::SeqCst);
        Some(state)
    }

    /// At launch: turn the setting on, once, if nothing was ever decided.
    /// Returns whether it was turned on now. The record is written first: if
    /// the app cannot remember, it does not turn anything on.
    pub fn apply_default(&self) -> Result<bool, String> {
        let _guard = self.changing.lock().unwrap();
        if self.record.exists() || !self.may_default { return Ok(false) }
        let before = self.system.state();
        self.record.write("default")?;
        // Already on (added by hand, or a previous copy), or held for approval
        // in System Settings: that is a choice already made.
        if before != State::Off { return Ok(false) }
        if let Err(error) = self.system.enable() {
            // Nothing was decided after all; the next launch tries again.
            self.record.clear();
            return Err(error);
        }
        Ok(true)
    }

    /// The menu item: the opposite of what the system says now. Returns the
    /// state afterwards, which is what the check mark shows.
    pub fn toggle(&self) -> Result<State, String> {
        let _guard = self.changing.lock().unwrap();
        let before = self.system.state();
        // Without the record, the next launch would apply the default again.
        self.record.write("menu")?;
        if before == State::On {
            self.system.disable()?;
            return Ok(self.system.state());
        }
        let enabled = self.system.enable();
        let after = self.system.state();
        if after == State::NeedsApproval {
            // Only the user can allow it, and only in System Settings.
            self.system.open_settings();
            return Ok(after);
        }
        enabled.map(|()| after)
    }

    /// The background service was removed: SintBot is not to start at login
    /// any more, and a login item that is on would still bring the app's icon
    /// back every time, so it goes too. The record goes with it:
    /// opening the app again sets everything up as on a new Mac. A setting the
    /// user had turned off stays off.
    pub fn service_removed(&self) -> Result<bool, String> {
        let _guard = self.changing.lock().unwrap();
        if self.system.state() != State::On { return Ok(false) }
        self.system.disable()?;
        self.record.clear();
        Ok(true)
    }
}

/// Which login item this copy of the app uses.
#[derive(Debug, PartialEq)]
pub enum Setup {
    /// The system's. `may_default` when the app runs from an Applications folder.
    System { may_default: bool },
    /// A development run that asked for the stand-in file.
    Simulated,
    /// No menu item and nothing registered. The reason goes to the log.
    Unavailable(&'static str),
}

/// `service`: the system has the login item service (macOS 13 and later).
/// `bundle`: the app bundle this runs from, when it is one that can be
/// launched at login (not a bare binary, not on a disk image or a randomized
/// read-only copy). A development run never touches the system's setting:
/// its bundle carries the same identifier as the installed app.
pub fn setup(service: bool, bundle: Option<&Path>, development: bool, simulated: bool) -> Setup {
    if development {
        return if simulated { Setup::Simulated } else { Setup::Unavailable("development run") };
    }
    if !service { return Setup::Unavailable("this macOS has no login item service (macOS 13 or later)") }
    match bundle {
        None => Setup::Unavailable("not running from an app bundle that can open at login"),
        Some(bundle) => Setup::System { may_default: in_applications(bundle) },
    }
}

/// `/Applications/SintBot.app`, `~/Applications/Tools/SintBot.app` and the like.
pub fn in_applications(bundle: &Path) -> bool {
    bundle.ancestors().skip(1).any(|dir| dir.file_name().is_some_and(|name| name == "Applications"))
}

const fn code(bytes: &[u8; 4]) -> u32 { u32::from_be_bytes(*bytes) }
const OPEN_APPLICATION: u32 = code(b"oapp");
const PROP_DATA: u32 = code(b"prdt");
const LAUNCHED_AS_LOGIN_ITEM: u32 = code(b"lgit");

/// The open-application event the system sends when it opens a login item.
pub fn is_login_event(event: u32, property: Option<u32>) -> bool {
    event == OPEN_APPLICATION && property == Some(LAUNCHED_AS_LOGIN_ITEM)
}

/// Must be called while the app finishes launching (Tauri's `setup`), when the
/// event that opened it is still the current one.
pub fn launched_at_login(development: bool) -> bool {
    if development && std::env::var(SIMULATE_LAUNCH_ENV).is_ok_and(|value| value == "1") { return true }
    #[cfg(target_os = "macos")]
    { mac::launched_at_login() }
    #[cfg(not(target_os = "macos"))]
    { false }
}

/// The system's service, when this macOS has one.
pub fn system_service() -> Option<Box<dyn System>> {
    #[cfg(target_os = "macos")]
    { mac::MainApp::get().map(|service| Box::new(service) as Box<dyn System>) }
    #[cfg(not(target_os = "macos"))]
    { None }
}

/// Stands in for System Settings in a development run: the file holds `on`,
/// `off` or `needs-approval`, and editing it is turning the switch there.
pub struct Simulated { pub file: PathBuf }

impl System for Simulated {
    fn state(&self) -> State { fs::read_to_string(&self.file).map(|text| State::parse(&text)).unwrap_or(State::Off) }
    fn enable(&self) -> Result<(), String> { self.set("on") }
    fn disable(&self) -> Result<(), String> { self.set("off") }
    fn open_settings(&self) { eprintln!("SintBot: (simulated) System Settings › Login Items: {}", self.file.display()); }
}

impl Simulated {
    fn set(&self, state: &str) -> Result<(), String> {
        if let Some(dir) = self.file.parent() { fs::create_dir_all(dir).map_err(|e| e.to_string())?; }
        fs::write(&self.file, format!("{state}\n")).map_err(|e| format!("{}: {e}", self.file.display()))
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::CStr;
    use std::os::raw::c_char;

    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::{AnyClass, NSObject};
    use objc2::msg_send;

    use super::{State, System};

    #[link(name = "ServiceManagement", kind = "framework")]
    extern "C" {}

    /// `SMAppService.mainApp`. Looked up by name: the class is missing before
    /// macOS 13, and then there is no service at all.
    pub struct MainApp { class: &'static AnyClass }

    impl MainApp {
        pub fn get() -> Option<Self> { AnyClass::get(c"SMAppService").map(|class| MainApp { class }) }

        fn with<T>(&self, f: impl FnOnce(&NSObject) -> T) -> Option<T> {
            autoreleasepool(|_| {
                // SAFETY: `mainAppService` is a class property returning an object (or nil).
                let service: Option<Retained<NSObject>> = unsafe { msg_send![self.class, mainAppService] };
                service.map(|service| f(&service))
            })
        }
    }

    fn describe(error: &NSObject) -> String {
        // SAFETY: an NSError; `localizedDescription` is an NSString, `UTF8String`
        // a NUL-terminated buffer that lives as long as the autorelease pool.
        unsafe {
            let code: isize = msg_send![error, code];
            let text: Option<Retained<NSObject>> = msg_send![error, localizedDescription];
            let text = text.map(|text| {
                let utf8: *const c_char = msg_send![&*text, UTF8String];
                if utf8.is_null() { String::new() } else { CStr::from_ptr(utf8).to_string_lossy().into_owned() }
            }).unwrap_or_default();
            format!("{text} (code {code})").trim().to_string()
        }
    }

    impl System for MainApp {
        fn state(&self) -> State {
            // SAFETY: `status` is an NSInteger property (SMAppServiceStatus).
            let status = self.with(|service| unsafe { msg_send![service, status] }).unwrap_or(3isize);
            match status {
                1 => State::On,
                2 => State::NeedsApproval,
                // 0 not registered; 3 not found, which is also what removing it
                // in System Settings can leave behind.
                _ => State::Off,
            }
        }
        fn enable(&self) -> Result<(), String> {
            self.with(|service| {
                // SAFETY: `registerAndReturnError:` returns BOOL and fills an NSError on failure.
                let result: Result<(), Retained<NSObject>> = unsafe { msg_send![service, registerAndReturnError: _] };
                result.map_err(|error| describe(&error))
            }).unwrap_or_else(|| Err("the login item service is not available".into()))
        }
        fn disable(&self) -> Result<(), String> {
            self.with(|service| {
                // SAFETY: as above.
                let result: Result<(), Retained<NSObject>> = unsafe { msg_send![service, unregisterAndReturnError: _] };
                result.map_err(|error| describe(&error))
            }).unwrap_or_else(|| Err("the login item service is not available".into()))
        }
        fn open_settings(&self) {
            // SAFETY: a class method without arguments or result.
            autoreleasepool(|_| unsafe { let () = msg_send![self.class, openSystemSettingsLoginItems]; });
        }
    }

    pub fn launched_at_login() -> bool {
        autoreleasepool(|_| {
            let Some(manager) = AnyClass::get(c"NSAppleEventManager") else { return false };
            // SAFETY: Foundation's shared Apple event manager; `currentAppleEvent`
            // is nil outside the handling of an event. The codes are FourCharCodes.
            unsafe {
                let manager: Option<Retained<NSObject>> = msg_send![manager, sharedAppleEventManager];
                let Some(manager) = manager else { return false };
                let event: Option<Retained<NSObject>> = msg_send![&*manager, currentAppleEvent];
                let Some(event) = event else { return false };
                let id: u32 = msg_send![&*event, eventID];
                let property: Option<Retained<NSObject>> = msg_send![&*event, paramDescriptorForKeyword: super::PROP_DATA];
                let value = property.map(|property| { let value: u32 = msg_send![&*property, enumCodeValue]; value });
                super::is_login_event(id, value)
            }
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// What System Settings would show, plus a count of what the app asked for.
    #[derive(Default)]
    pub struct Fake {
        pub state: Mutex<Option<State>>,
        pub enabled: AtomicUsize,
        pub disabled: AtomicUsize,
        pub settings_opened: AtomicUsize,
        /// `enable` leaves it waiting for approval instead of on.
        pub approval: AtomicBool,
        pub fail: Mutex<Option<String>>,
    }

    impl Fake {
        pub fn set(&self, state: State) { *self.state.lock().unwrap() = Some(state); }
        fn failure(&self) -> Result<(), String> { self.fail.lock().unwrap().clone().map_or(Ok(()), Err) }
    }

    impl System for Arc<Fake> {
        fn state(&self) -> State { self.state.lock().unwrap().unwrap_or(State::Off) }
        fn enable(&self) -> Result<(), String> {
            self.enabled.fetch_add(1, Ordering::SeqCst);
            self.failure()?;
            self.set(if self.approval.load(Ordering::SeqCst) { State::NeedsApproval } else { State::On });
            Ok(())
        }
        fn disable(&self) -> Result<(), String> {
            self.disabled.fetch_add(1, Ordering::SeqCst);
            self.failure()?;
            self.set(State::Off);
            Ok(())
        }
        fn open_settings(&self) { self.settings_opened.fetch_add(1, Ordering::SeqCst); }
    }

    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sintbot-desktop-login-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn fake_item(dir: &Path, may_default: bool) -> (Arc<Fake>, LoginItem) {
        let fake = Arc::new(Fake::default());
        let item = LoginItem::new(Box::new(fake.clone()), Record::new(dir.join("login-item.json")), may_default);
        (fake, item)
    }

    #[test]
    fn the_default_is_applied_once() {
        let dir = scratch("once");
        let (fake, item) = fake_item(&dir, true);
        assert_eq!(item.apply_default(), Ok(true));
        assert_eq!(item.state(), State::On);
        assert!(dir.join("login-item.json").is_file());
        // Every later launch leaves it alone, whatever the system says.
        assert_eq!(item.apply_default(), Ok(false));
        fake.set(State::Off);
        assert_eq!(item.apply_default(), Ok(false));
        assert_eq!(fake.enabled.load(Ordering::SeqCst), 1);
        // A new copy of the app (an update, a reinstall) reads the same record.
        let (other, again) = fake_item(&dir, true);
        assert_eq!(again.apply_default(), Ok(false));
        assert_eq!(other.enabled.load(Ordering::SeqCst), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn turned_off_in_system_settings_stays_off() {
        let dir = scratch("settings");
        let (fake, item) = fake_item(&dir, true);
        item.apply_default().unwrap();
        // The user removes SintBot from Open at Login.
        fake.set(State::Off);
        for _ in 0..3 { assert_eq!(item.apply_default(), Ok(false)); }
        assert_eq!(item.state(), State::Off);
        assert_eq!(fake.enabled.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn turned_off_in_the_menu_stays_off() {
        let dir = scratch("menu");
        let (fake, item) = fake_item(&dir, true);
        item.apply_default().unwrap();
        assert_eq!(item.toggle(), Ok(State::Off));
        assert_eq!(item.apply_default(), Ok(false));
        assert_eq!((fake.enabled.load(Ordering::SeqCst), fake.disabled.load(Ordering::SeqCst)), (1, 1));
        // And back on from the menu.
        assert_eq!(item.toggle(), Ok(State::On));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_choice_from_the_menu_counts_before_the_default_ran() {
        // Opened from the disk image first: no default there, but the user's
        // choice is remembered for the copy in Applications.
        let dir = scratch("early");
        let (fake, from_image) = fake_item(&dir, false);
        assert_eq!(from_image.apply_default(), Ok(false));
        assert!(!dir.join("login-item.json").exists(), "nothing decided yet");
        assert_eq!(from_image.toggle(), Ok(State::On));
        assert_eq!(from_image.toggle(), Ok(State::Off));
        let (_, installed) = fake_item(&dir, true);
        assert_eq!(installed.apply_default(), Ok(false));
        assert_eq!(fake.enabled.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_setting_is_a_choice_already_made() {
        let dir = scratch("existing");
        let (fake, item) = fake_item(&dir, true);
        fake.set(State::On);
        assert_eq!(item.apply_default(), Ok(false));
        fake.set(State::Off);
        assert_eq!(item.apply_default(), Ok(false), "remembered although the app did not turn it on");
        fs::remove_dir_all(&dir).unwrap();
        let dir = scratch("held");
        let (fake, held) = fake_item(&dir, true);
        fake.set(State::NeedsApproval);
        assert_eq!(held.apply_default(), Ok(false));
        assert_eq!(fake.enabled.load(Ordering::SeqCst), 0, "held for approval: not asked again");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_default_is_tried_again_next_time() {
        let dir = scratch("failed");
        let (fake, item) = fake_item(&dir, true);
        *fake.fail.lock().unwrap() = Some("refused".into());
        assert_eq!(item.apply_default(), Err("refused".into()));
        assert!(!dir.join("login-item.json").exists());
        *fake.fail.lock().unwrap() = None;
        assert_eq!(item.apply_default(), Ok(true));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_is_turned_on_when_the_choice_cannot_be_remembered() {
        let dir = scratch("unwritable");
        // A file where the record's directory would have to be.
        fs::write(dir.join("blocked"), "").unwrap();
        let fake = Arc::new(Fake::default());
        let item = LoginItem::new(Box::new(fake.clone()), Record::new(dir.join("blocked/login-item.json")), true);
        assert!(item.apply_default().is_err());
        assert!(item.toggle().is_err());
        assert_eq!((fake.enabled.load(Ordering::SeqCst), fake.disabled.load(Ordering::SeqCst)), (0, 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_check_mark_follows_the_system() {
        let dir = scratch("check");
        let (fake, item) = fake_item(&dir, true);
        item.apply_default().unwrap();
        assert!(item.try_state().unwrap().checked());
        // Changed in System Settings while the app runs: the next read shows it.
        fake.set(State::Off);
        assert!(!item.try_state().unwrap().checked());
        fake.set(State::NeedsApproval);
        assert!(!item.try_state().unwrap().checked(), "waiting for approval is not on");
        // The menu item acts on what the system says, not on a remembered mark.
        assert_eq!(item.toggle(), Ok(State::On));
        fake.set(State::Off);
        assert_eq!(item.toggle(), Ok(State::On));
        assert_eq!(fake.disabled.load(Ordering::SeqCst), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn approval_is_given_in_system_settings() {
        let dir = scratch("approval");
        let (fake, item) = fake_item(&dir, true);
        fake.approval.store(true, Ordering::SeqCst);
        assert_eq!(item.toggle(), Ok(State::NeedsApproval));
        assert_eq!(fake.settings_opened.load(Ordering::SeqCst), 1);
        // An error on the way to approval is not reported as a failure either.
        fake.set(State::NeedsApproval);
        *fake.fail.lock().unwrap() = Some("Operation not permitted".into());
        assert_eq!(item.toggle(), Ok(State::NeedsApproval));
        assert_eq!(fake.settings_opened.load(Ordering::SeqCst), 2);
        // Any other failure is.
        fake.set(State::Off);
        fake.approval.store(false, Ordering::SeqCst);
        assert_eq!(item.toggle(), Err("Operation not permitted".into()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removing_the_service_takes_the_login_item_along() {
        let dir = scratch("remove");
        let (fake, item) = fake_item(&dir, true);
        item.apply_default().unwrap();
        assert_eq!(item.service_removed(), Ok(true));
        assert_eq!(item.state(), State::Off);
        // Opening the app again sets it up as on a new Mac.
        assert_eq!(item.apply_default(), Ok(true));
        // A setting the user turned off stays off through a removal.
        item.toggle().unwrap();
        assert_eq!(item.service_removed(), Ok(false));
        assert_eq!(item.apply_default(), Ok(false));
        assert_eq!(fake.enabled.load(Ordering::SeqCst), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn which_login_item_a_copy_uses() {
        let app = Path::new("/Applications/SintBot.app");
        assert_eq!(setup(true, Some(app), false, false), Setup::System { may_default: true });
        // macOS 12: no service, no menu item, nothing registered.
        assert!(matches!(setup(false, Some(app), false, false), Setup::Unavailable(_)));
        // A bare binary, a disk image, a randomized copy.
        assert!(matches!(setup(true, None, false, false), Setup::Unavailable(_)));
        // Somewhere else: the menu item works, but the app does not turn it on by itself.
        assert_eq!(setup(true, Some(Path::new("/Users/a/Downloads/SintBot.app")), false, false), Setup::System { may_default: false });
        // A development run never uses the system's setting, simulated or not.
        assert!(matches!(setup(true, Some(app), true, false), Setup::Unavailable(_)));
        assert_eq!(setup(true, Some(app), true, true), Setup::Simulated);
        assert_eq!(setup(false, None, true, true), Setup::Simulated);
    }

    #[test]
    fn applications_folders() {
        for yes in ["/Applications/SintBot.app", "/Users/a/Applications/SintBot.app", "/Applications/Tools/SintBot.app"] {
            assert!(in_applications(Path::new(yes)), "{yes}");
        }
        for no in ["/Users/a/Downloads/SintBot.app", "/Volumes/SintBot/SintBot.app",
            "/src/sintbot-desktop/src-tauri/target/release/bundle/macos/SintBot.app", "/Applications.app"] {
            assert!(!in_applications(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn a_login_launch_is_recognised_by_its_open_event() {
        assert!(is_login_event(code(b"oapp"), Some(code(b"lgit"))));
        // Opened by the user (no property), reopened as a service item, or opening a document.
        assert!(!is_login_event(code(b"oapp"), None));
        assert!(!is_login_event(code(b"oapp"), Some(code(b"svit"))));
        assert!(!is_login_event(code(b"odoc"), Some(code(b"lgit"))));
        assert_eq!(code(b"oapp"), 0x6f61_7070);
    }

    /// Introspection only: nothing is registered and no login setting is read.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_messages_sent_to_the_system_exist_with_these_types() {
        use objc2::runtime::{AnyClass, AnyObject, Bool};
        use objc2::sel;
        let class = |name: &std::ffi::CStr| AnyClass::get(name).unwrap_or_else(|| panic!("{name:?}"));
        let events = class(c"NSAppleEventManager");
        assert_eq!(events.metaclass().verify_sel::<(), &AnyObject>(sel!(sharedAppleEventManager)), Ok(()));
        assert_eq!(events.verify_sel::<(), &AnyObject>(sel!(currentAppleEvent)), Ok(()));
        let descriptor = class(c"NSAppleEventDescriptor");
        assert_eq!(descriptor.verify_sel::<(), u32>(sel!(eventID)), Ok(()));
        assert_eq!(descriptor.verify_sel::<(u32,), &AnyObject>(sel!(paramDescriptorForKeyword:)), Ok(()));
        assert_eq!(descriptor.verify_sel::<(), u32>(sel!(enumCodeValue)), Ok(()));
        let error = class(c"NSError");
        assert_eq!(error.verify_sel::<(), isize>(sel!(code)), Ok(()));
        assert_eq!(error.verify_sel::<(), &AnyObject>(sel!(localizedDescription)), Ok(()));
        assert_eq!(class(c"NSString").verify_sel::<(), *const std::os::raw::c_char>(sel!(UTF8String)), Ok(()));

        let version = std::process::Command::new("/usr/bin/sw_vers").arg("-productVersion").output().unwrap();
        let major: u32 = String::from_utf8_lossy(&version.stdout).split('.').next().unwrap().trim().parse().unwrap();
        let Some(service) = AnyClass::get(c"SMAppService") else {
            assert!(major < 13, "macOS {major} has SMAppService; the framework is not linked");
            return;
        };
        assert!(system_service().is_some());
        assert_eq!(service.metaclass().verify_sel::<(), &AnyObject>(sel!(mainAppService)), Ok(()));
        assert_eq!(service.metaclass().verify_sel::<(), ()>(sel!(openSystemSettingsLoginItems)), Ok(()));
        assert_eq!(service.verify_sel::<(), isize>(sel!(status)), Ok(()));
        for name in [sel!(registerAndReturnError:), sel!(unregisterAndReturnError:)] {
            assert_eq!(service.verify_sel::<(*mut *mut AnyObject,), Bool>(name), Ok(()), "{name:?}");
        }
    }

    #[test]
    fn a_process_not_opened_at_login_is_not_taken_for_one() {
        // This test process was not opened by an Apple event at all.
        assert!(!launched_at_login(false));
        // The simulation is only for development runs.
        std::env::set_var(SIMULATE_LAUNCH_ENV, "1");
        let (installed, development) = (launched_at_login(false), launched_at_login(true));
        std::env::remove_var(SIMULATE_LAUNCH_ENV);
        assert!(!installed && development);
    }

    #[test]
    fn the_simulated_switch_is_a_file() {
        let dir = scratch("simulated");
        let simulated = Simulated { file: dir.join("login-item-simulated") };
        assert_eq!(simulated.state(), State::Off);
        simulated.enable().unwrap();
        assert_eq!(simulated.state(), State::On);
        fs::write(&simulated.file, "needs-approval\n").unwrap();
        assert_eq!(simulated.state(), State::NeedsApproval);
        simulated.disable().unwrap();
        assert_eq!(simulated.state(), State::Off);
        fs::remove_dir_all(&dir).unwrap();
    }
}
