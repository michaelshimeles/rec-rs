//! Moving the app into Applications. macOS runs a downloaded app opened where it landed (on its
//! disk image, or in Downloads before Finder has moved it) from a random read-only folder: App
//! Translocation. An app on a disk image is read-only too. Sparkle can't replace a copy in either
//! place, so Rec offers to move itself into Applications and reopen from there.

use std::ffi::{CString, OsString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication};
use objc2_foundation::{NSBundle, NSFileManager, NSURL};

use crate::util::ns;

/// Whether the running app needs moving: translocated or on a read-only volume. Checked once.
pub fn needs_move() -> bool {
    static NEEDS: OnceLock<bool> = OnceLock::new();
    *NEEDS.get_or_init(|| bundle().is_some_and(|b| is_translocated(&b) || read_only(&b)))
}

/// The running .app bundle (None for a bare binary, as `cargo run` builds one).
fn bundle() -> Option<PathBuf> {
    let p = PathBuf::from(NSBundle::mainBundle().bundlePath().to_string());
    (p.extension()? == "app").then_some(p)
}

/// Security.framework has exported these since macOS 10.12, without a public header.
type IsTranslocatedUrl = unsafe extern "C" fn(*const c_void, *mut bool, *mut *mut c_void) -> bool;
type CreateOriginalPath = unsafe extern "C" fn(*const c_void, *mut *mut c_void) -> *mut c_void;

fn security_fn(name: &std::ffi::CStr) -> Option<*mut c_void> {
    let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!f.is_null()).then_some(f)
}

fn file_url(p: &Path) -> Option<Retained<NSURL>> {
    Some(NSURL::fileURLWithPath(&ns(p.to_str()?)))
}

fn is_translocated(p: &Path) -> bool {
    let by_api = security_fn(c"SecTranslocateIsTranslocatedURL").zip(file_url(p)).is_some_and(|(f, url)| {
        let f: IsTranslocatedUrl = unsafe { std::mem::transmute::<*mut c_void, IsTranslocatedUrl>(f) };
        let mut translocated = false;
        // SAFETY: an NSURL is toll-free bridged to the CFURL the function takes.
        let ok = unsafe { f(Retained::as_ptr(&url).cast(), &mut translocated, std::ptr::null_mut()) };
        ok && translocated
    });
    by_api || p.components().any(|c| c.as_os_str() == "AppTranslocation")
}

/// Where a translocated bundle really is: on its disk image, or in Downloads.
fn original_path(p: &Path) -> Option<PathBuf> {
    let f = security_fn(c"SecTranslocateCreateOriginalPathForURL")?;
    let f: CreateOriginalPath = unsafe { std::mem::transmute::<*mut c_void, CreateOriginalPath>(f) };
    let url = file_url(p)?;
    // SAFETY: as above; the function returns a +1 CFURL (an NSURL), or NULL.
    let original = unsafe { f(Retained::as_ptr(&url).cast(), std::ptr::null_mut()) };
    let original: Retained<NSURL> = unsafe { Retained::from_raw(original.cast())? };
    Some(PathBuf::from(original.path()?.to_string()))
}

fn read_only(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { return false };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c.as_ptr(), &mut st) == 0 && st.f_flags & libc::MNT_RDONLY as u32 != 0 }
}

/// /Applications, or ~/Applications for a user who can't write there. REC_MOVE_TO overrides it
/// (self-tests; see `trash`).
fn applications_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("REC_MOVE_TO") {
        return Ok(PathBuf::from(d));
    }
    if unsafe { libc::access(c"/Applications".as_ptr(), libc::W_OK) } == 0 {
        return Ok(PathBuf::from("/Applications"));
    }
    let home = std::env::var_os("HOME").context("no home folder")?;
    let d = PathBuf::from(home).join("Applications");
    std::fs::create_dir_all(&d).with_context(|| format!("could not create {}", d.display()))?;
    Ok(d)
}

/// Copy `bundle` into `apps` under its own name, without the quarantine flag (with it, macOS
/// would translocate the copy as well). An older copy there is swapped out and handed to
/// `discard`; if that fails, it is swapped back. Returns the new bundle's path.
pub fn install(bundle: &Path, apps: &Path, discard: impl Fn(&Path) -> Result<()>) -> Result<PathBuf> {
    let name = bundle.file_name().context("the app has no file name")?;
    let dest = apps.join(name);
    // Staged in a folder of this process's own (another copy may be moving itself at the same
    // time), under the app's name, which an old copy swapped out keeps in the Trash.
    let mut stage_name = OsString::from(".");
    stage_name.push(name);
    stage_name.push(format!(".moving-{}", std::process::id()));
    let stage = apps.join(stage_name);
    let tmp = stage.join(name);
    let _ = std::fs::remove_dir_all(&stage);
    let copied = Command::new("/usr/bin/ditto").arg(bundle).arg(&tmp).status().context("could not run ditto")?;
    let placed = (|| {
        if !copied.success() {
            bail!("could not copy {} into {}", bundle.display(), apps.display());
        }
        strip_quarantine(&tmp)?;
        // The destination is never missing, even for a moment: another copy may be opening it.
        match rename(&tmp, &dest, libc::RENAME_EXCL) {
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                rename(&tmp, &dest, libc::RENAME_SWAP).with_context(|| format!("could not replace {}", dest.display()))?;
                if let Err(e) = discard(&tmp) {
                    let _ = rename(&tmp, &dest, libc::RENAME_SWAP);
                    return Err(e.context(format!("could not replace {}", dest.display())));
                }
                Ok(())
            }
            r => r.with_context(|| format!("could not move the copy to {}", dest.display())),
        }
    })();
    let _ = std::fs::remove_dir_all(&stage);
    placed.map(|()| dest)
}

/// rename(2) with RENAME_EXCL (fail if `to` exists) or RENAME_SWAP (exchange the two).
fn rename(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    let (f, t) = (CString::new(from.as_os_str().as_bytes())?, CString::new(to.as_os_str().as_bytes())?);
    if unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), flags) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn strip_quarantine(p: &Path) -> Result<()> {
    let c = CString::new(p.as_os_str().as_bytes())?;
    if unsafe { libc::removexattr(c.as_ptr(), c"com.apple.quarantine".as_ptr(), libc::XATTR_NOFOLLOW) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOATTR) {
            return Err(e).with_context(|| format!("could not clear the quarantine flag of {}", p.display()));
        }
    }
    if std::fs::symlink_metadata(p)?.is_dir() {
        for entry in std::fs::read_dir(p)? {
            strip_quarantine(&entry?.path())?;
        }
    }
    Ok(())
}

/// Into the Trash; with REC_MOVE_TO (self-tests), into a Trash folder beside that instead.
fn trash(p: &Path) -> Result<()> {
    if let Some(d) = std::env::var_os("REC_MOVE_TO") {
        let bin = PathBuf::from(d).with_file_name("Trash");
        std::fs::create_dir_all(&bin)?;
        let name = format!("{}-{}", std::fs::read_dir(&bin)?.count(), p.file_name().unwrap_or_default().to_string_lossy());
        return Ok(std::fs::rename(p, bin.join(name))?);
    }
    let url = file_url(p).context("path isn't UTF-8")?;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))
}

/// Open `app` once this process has exited, as it was started.
fn reopen(app: &Path) -> Result<()> {
    let mut pids = vec![std::process::id().to_string()];
    // Started from a shell, Rec re-runs itself as a child of the copy that was started
    // (tcc::disclaim_and_reexec), which exits after it.
    if std::env::var_os("REC_DISCLAIMED").is_some() {
        pids.push(unsafe { libc::getppid() }.to_string());
    }
    let background = std::env::var_os("REC_GUI_BACKGROUND").is_some();
    let args = std::env::args_os().skip(1).filter(|a| !a.as_bytes().starts_with(b"-psn_")).collect();
    let open = open_args(app, std::env::vars_os(), args, background);
    let script = r#"for pid in $1; do while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done; done; shift; exec /usr/bin/open "$@""#;
    Command::new("/bin/sh").arg("-c").arg(script).arg("sh").arg(pids.join(" ")).args(open).spawn().context("could not reopen Rec")?;
    Ok(())
}

/// `open` arguments that start a new instance of `app` (another copy of Rec may be running), with
/// this one's command line (`gui --start-recording`, say) and REC_* settings (self-tests).
fn open_args(app: &Path, env: impl IntoIterator<Item = (OsString, OsString)>, args: Vec<OsString>, background: bool) -> Vec<OsString> {
    let mut open: Vec<OsString> = vec!["-n".into()];
    for (k, v) in env {
        if k.as_bytes().starts_with(b"REC_") && k != "REC_DISCLAIMED" {
            let mut kv = k;
            kv.push("=");
            kv.push(v);
            open.extend(["--env".into(), kv]);
        }
    }
    if background {
        open.push("-g".into());
    }
    open.push(app.into());
    if !args.is_empty() {
        open.push("--args".into());
        open.extend(args);
    }
    open
}

/// What makes a copy that needs moving updatable.
#[derive(Debug, PartialEq)]
enum Plan {
    /// Translocated although it is in an Applications folder (it was copied there, not moved in
    /// Finder): clear the quarantine flag of that copy and reopen it.
    Clear(PathBuf),
    /// Copy into `apps` and reopen from there, then put `trash` (a download in Downloads, say)
    /// in the Trash.
    Move { apps: PathBuf, trash: Option<PathBuf> },
}

/// `original` is the translocated copy's real location, if known; `apps` is where a move goes.
fn plan(original: Option<&Path>, apps: &Path, writable: impl Fn(&Path) -> bool) -> Plan {
    let home_apps = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Applications"));
    let in_applications = |p: &Path| {
        p.parent().is_some_and(|d| same(d, apps) || same(d, Path::new("/Applications")) || home_apps.as_deref().is_some_and(|h| same(d, h)))
    };
    let dest = original.and_then(Path::file_name).map(|n| apps.join(n));
    match original {
        Some(o) if writable(o) && in_applications(o) => Plan::Clear(o.to_path_buf()),
        Some(o) if writable(o) && !dest.is_some_and(|d| same(o, &d)) => Plan::Move { apps: apps.to_path_buf(), trash: Some(o.to_path_buf()) },
        _ => Plan::Move { apps: apps.to_path_buf(), trash: None },
    }
}

/// Whether two paths name the same place: macOS reports real paths (/private/tmp), which may be
/// spelled through a symlink elsewhere (/tmp).
fn same(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    real(a) == real(b)
}

fn writable(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { return false };
    !read_only(p) && unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0
}

/// Offer to make the app updatable: move it into Applications, or clear the quarantine flag of
/// the copy already there. Returns true once the app will reopen from its new place when this
/// process exits: quit then. REC_GUI_SELFTEST=move accepts without asking.
pub fn offer_move() -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let Some(bundle) = bundle() else { return false };
    let selftest = std::env::var("REC_GUI_SELFTEST").as_deref() == Ok("move");
    let apps = match applications_dir() {
        Ok(d) => d,
        Err(e) => {
            failed(mtm, &e, selftest);
            return false;
        }
    };
    let plan = plan(original_path(&bundle).as_deref(), &apps, writable);
    if !selftest && !ask(mtm, &plan, bundle.file_name().is_some_and(|n| apps.join(n).exists())) {
        return false;
    }
    let done = match &plan {
        Plan::Clear(app) => strip_quarantine(app).and_then(|()| reopen(app)).map(|()| app.clone()),
        Plan::Move { apps, trash: original } => install(&bundle, apps, trash).and_then(|dest| {
            reopen(&dest)?;
            if let Some(o) = original.as_deref().filter(|o| !same(o, &dest))
                && let Err(e) = trash(o)
            {
                eprintln!("rec: moved, but could not put {} in the Trash: {e:#}", o.display());
            }
            Ok(dest)
        }),
    };
    match done {
        Ok(at) => {
            eprintln!("rec: reopening from {}", at.display());
            if selftest {
                println!("selftest: {plan:?} from={} reopen={}", bundle.display(), at.display());
            }
            true
        }
        Err(e) => {
            failed(mtm, &e, selftest);
            false
        }
    }
}

fn ask(mtm: MainThreadMarker, plan: &Plan, replaces: bool) -> bool {
    let alert = NSAlert::new(mtm);
    let (title, info, button) = match plan {
        Plan::Clear(_) => (
            "Reopen Rec so it can install updates?".to_string(),
            "macOS runs this copy of Rec from a temporary read-only folder because it wasn't moved in Finder after downloading. Rec can't update itself there. Rec can fix this and reopen.".to_string(),
            "Fix and Reopen".to_string(),
        ),
        Plan::Move { apps, .. } => {
            let mut info = String::from(
                "Rec is running from its disk image or from where it was downloaded, and it can't update itself there. Rec can move itself and reopen.",
            );
            if replaces {
                info.push_str(" The Rec already there goes to the Trash.");
            }
            (format!("Move Rec to {}?", folder_name(apps)), info, format!("Move to {}", folder_name(apps)))
        }
    };
    alert.setMessageText(&ns(&title));
    alert.setInformativeText(&ns(&info));
    alert.addButtonWithTitle(&ns(&button));
    alert.addButtonWithTitle(&ns("Not Now"));
    NSApplication::sharedApplication(mtm).activate();
    alert.runModal() == NSAlertFirstButtonReturn
}

fn failed(mtm: MainThreadMarker, e: &anyhow::Error, selftest: bool) {
    eprintln!("rec: could not make Rec updatable: {e:#}");
    if selftest {
        println!("selftest: failed: {e:#}");
        return;
    }
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&ns("Rec couldn't fix this"));
    alert.setInformativeText(&ns(&format!(
        "{e:#}\n\nQuit Rec, drag it to Applications in Finder, and open it from there."
    )));
    alert.runModal();
}

/// "Applications in your home folder" for ~/Applications, "Applications" for /Applications
/// (or another folder by that name).
fn folder_name(apps: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.is_some_and(|h| same(apps, &h.join("Applications"))) {
        "Applications in your home folder".into()
    } else if apps.file_name().is_some_and(|n| n == "Applications") {
        "Applications".into()
    } else {
        apps.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quarantined(p: &Path) -> bool {
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        unsafe { libc::getxattr(c.as_ptr(), c"com.apple.quarantine".as_ptr(), std::ptr::null_mut(), 0, 0, libc::XATTR_NOFOLLOW) >= 0 }
    }

    fn quarantine(p: &Path) {
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        let v = b"0083;6716f0a1;Safari;";
        let r = unsafe { libc::setxattr(c.as_ptr(), c"com.apple.quarantine".as_ptr(), v.as_ptr().cast(), v.len(), 0, libc::XATTR_NOFOLLOW) };
        assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
    }

    /// A small bundle like Rec's: a binary, a framework with a symlinked version, quarantined
    /// throughout as a browser leaves a download.
    fn fake_bundle(dir: &Path, version: &str) -> PathBuf {
        let app = dir.join("Rec.app");
        let fw = app.join("Contents/Frameworks/Sparkle.framework");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(fw.join("Versions/B")).unwrap();
        std::fs::write(app.join("Contents/MacOS/rec-rs"), version).unwrap();
        std::fs::write(fw.join("Versions/B/Sparkle"), "sparkle").unwrap();
        std::os::unix::fs::symlink("B", fw.join("Versions/Current")).unwrap();
        for p in [&app, &app.join("Contents/MacOS/rec-rs"), &fw.join("Versions/B/Sparkle")] {
            quarantine(p);
        }
        app
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rec-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn installs_a_copy_without_quarantine() {
        let d = temp_dir("fresh");
        let (src, apps) = (d.join("Downloads"), d.join("Applications"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&apps).unwrap();
        let app = fake_bundle(&src, "new");
        let dest = install(&app, &apps, |_| panic!("nothing to replace")).unwrap();
        assert_eq!(dest, apps.join("Rec.app"));
        assert_eq!(std::fs::read_to_string(dest.join("Contents/MacOS/rec-rs")).unwrap(), "new");
        let current = dest.join("Contents/Frameworks/Sparkle.framework/Versions/Current");
        assert_eq!(std::fs::read_link(&current).unwrap(), Path::new("B"), "symlinks stay symlinks");
        assert!(!quarantined(&dest) && !quarantined(&dest.join("Contents/MacOS/rec-rs")));
        assert!(quarantined(&app), "the download itself is left alone");
        assert_eq!(std::fs::read_dir(&apps).unwrap().count(), 1, "no copy left behind");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn replaces_an_older_copy() {
        let d = temp_dir("replace");
        let (src, apps, old) = (d.join("Downloads"), d.join("Applications"), d.join("old"));
        for p in [&src, &apps, &old] {
            std::fs::create_dir_all(p).unwrap();
        }
        fake_bundle(&apps, "old");
        let app = fake_bundle(&src, "new");
        let dest = install(&app, &apps, |p| Ok(std::fs::rename(p, old.join("Rec.app"))?)).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("Contents/MacOS/rec-rs")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old.join("Rec.app/Contents/MacOS/rec-rs")).unwrap(), "old");
        // If the old copy can't be taken away, it stays as it was.
        let err = install(&app, &apps, |_| bail!("in use")).unwrap_err();
        assert!(format!("{err:#}").contains("could not replace") && format!("{err:#}").contains("in use"), "{err:#}");
        assert_eq!(std::fs::read_to_string(dest.join("Contents/MacOS/rec-rs")).unwrap(), "new");
        assert_eq!(std::fs::read_dir(&apps).unwrap().count(), 1, "no copy left behind");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn spots_read_only_and_translocated_apps() {
        // The sealed system volume is mounted read-only.
        assert!(read_only(Path::new("/System/Library")));
        assert!(!read_only(&std::env::temp_dir()));
        let t = "/private/var/folders/r2/x/T/AppTranslocation/0D121AD2-2488-4CED-B0B8-9B668440C0C7/d/Rec.app";
        assert!(is_translocated(Path::new(t)));
        assert!(!is_translocated(Path::new("/Applications/Rec.app")));
    }

    #[test]
    fn plans_by_where_the_download_really_is() {
        let apps = Path::new("/Applications");
        let ok = |_: &Path| true;
        let ro = |_: &Path| false;
        // Your case: copied into Applications, quarantined, translocated.
        assert_eq!(plan(Some(Path::new("/Applications/Rec.app")), apps, ok), Plan::Clear("/Applications/Rec.app".into()));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let user_apps = home.join("Applications/Rec.app");
        assert_eq!(plan(Some(&user_apps), apps, ok), Plan::Clear(user_apps.clone()));
        // Opened in Downloads: move, and the download goes to the Trash.
        let dl = home.join("Downloads/Rec.app");
        assert_eq!(plan(Some(&dl), apps, ok), Plan::Move { apps: apps.into(), trash: Some(dl.clone()) });
        // Opened on the disk image, or a copy Rec can't change: move, nothing to trash.
        assert_eq!(plan(Some(Path::new("/Volumes/Rec/Rec.app")), apps, ro), Plan::Move { apps: apps.into(), trash: None });
        assert_eq!(plan(Some(Path::new("/Applications/Rec.app")), apps, ro), Plan::Move { apps: apps.into(), trash: None });
        // Not translocated (a disk image without the quarantine flag), or the original unknown.
        assert_eq!(plan(None, apps, ok), Plan::Move { apps: apps.into(), trash: None });
    }

    #[test]
    fn compares_real_paths() {
        // macOS reports the real path of a translocated app (/private/tmp/...), which may not be
        // how the Applications folder is spelled (/tmp/...). The copy must be cleared in place,
        // never moved onto itself and then trashed.
        let d = temp_dir("symlink");
        std::fs::create_dir_all(d.join("real/Applications/Rec.app")).unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        let original = std::fs::canonicalize(d.join("real/Applications/Rec.app")).unwrap();
        let apps = d.join("link/Applications");
        assert_eq!(plan(Some(&original), &apps, |_| true), Plan::Clear(original.clone()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn never_leaves_the_destination_missing() {
        // Another copy may be opening the destination at any moment: the new copy goes in by an
        // atomic swap, and only then does the old one go, under its own name.
        let d = temp_dir("swap");
        let (src, apps) = (d.join("Downloads"), d.join("Applications"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&apps).unwrap();
        fake_bundle(&apps, "old");
        let app = fake_bundle(&src, "new");
        let dest = apps.join("Rec.app");
        let seen = std::cell::RefCell::new(vec![]);
        install(&app, &apps, |old| {
            seen.borrow_mut().push((
                std::fs::read_to_string(dest.join("Contents/MacOS/rec-rs")).unwrap_or_default(),
                std::fs::read_to_string(old.join("Contents/MacOS/rec-rs")).unwrap_or_default(),
                old.file_name().unwrap().to_owned(),
            ));
            Ok(std::fs::remove_dir_all(old)?)
        })
        .unwrap();
        assert_eq!(*seen.borrow(), [("new".to_string(), "old".to_string(), OsString::from("Rec.app"))]);
        assert_eq!(std::fs::read_dir(&apps).unwrap().count(), 1, "no copy left behind");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stages_apart_from_other_copies() {
        // Another running copy may be staging its own install in the same folder.
        let d = temp_dir("staging");
        let (src, apps) = (d.join("Downloads"), d.join("Applications"));
        std::fs::create_dir_all(apps.join(".Rec.app.moving/Contents")).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let app = fake_bundle(&src, "new");
        install(&app, &apps, |_| panic!("nothing to replace")).unwrap();
        assert!(apps.join(".Rec.app.moving/Contents").is_dir(), "another copy's staging is left alone");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reopens_a_new_instance_with_the_same_options() {
        let env = [("REC_SETTINGS", "/tmp/s.json"), ("REC_DISCLAIMED", "1"), ("HOME", "/Users/x")];
        let args = ["gui", "--start-recording"];
        let a = open_args(Path::new("/Applications/Rec.app"), env.map(|(k, v)| (k.into(), v.into())), args.map(Into::into).to_vec(), false);
        let a: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(a, ["-n", "--env", "REC_SETTINGS=/tmp/s.json", "/Applications/Rec.app", "--args", "gui", "--start-recording"]);
        let a = open_args(Path::new("/A/Rec.app"), vec![], vec![], true);
        assert_eq!(a, ["-n", "-g", "/A/Rec.app"].map(OsString::from));
    }

    /// Run against a real translocated copy: REC_TRANSLOCATED=<its bundle path> cargo test --
    /// --ignored original_of_a_translocated_app
    #[test]
    #[ignore]
    fn original_of_a_translocated_app() {
        let p = PathBuf::from(std::env::var("REC_TRANSLOCATED").expect("REC_TRANSLOCATED"));
        assert!(is_translocated(&p));
        let o = original_path(&p).expect("original path");
        println!("{} is really {} (read-only: {})", p.display(), o.display(), read_only(&o));
        assert!(!o.to_string_lossy().contains("AppTranslocation") && o.exists());
    }
}
