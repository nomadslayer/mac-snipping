//! Build tooling (pure Rust): `cargo xtask <bundle|install|run|icon>`.
//!
//!   bundle   Build release and assemble dist/Snipping.app (icon, Info.plist, signature)
//!   install  bundle + copy to /Applications (falls back to ~/Applications)
//!   run      bundle + launch the app
//!   dmg      bundle + dist/Snipping.dmg (drag-to-Applications installer)
//!   setup-signing   one-time: create + trust a local code-signing certificate
//!                   so Screen Recording permission survives rebuilds
//!   icon     only render the icon PNGs/icns into dist/

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LinearGradient, Paint, PathBuilder, Pixmap, Point, SpreadMode, Stroke,
    StrokeDash, Transform,
};
type SkPath = tiny_skia::Path;

const APP_NAME: &str = "Snipping";
const BUNDLE_ID: &str = "com.snipping.app";
const EXECUTABLE: &str = "snipping";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn version() -> String {
    let toml = fs::read_to_string(root().join("Cargo.toml")).unwrap_or_default();
    toml.lines()
        .find_map(|l| l.strip_prefix("version = \"").map(|v| v.trim_end_matches('"').to_string()))
        .unwrap_or_else(|| "1.0.0".into())
}

fn run(cmd: &mut Command) {
    // Echo the command, but never secrets (keychain/PKCS#12 passwords).
    let mut redact_next = false;
    let shown: Vec<String> = std::iter::once(cmd.get_program().to_string_lossy().into_owned())
        .chain(cmd.get_args().map(|a| {
            let a = a.to_string_lossy().into_owned();
            let out = if redact_next || a.starts_with("pass:") { "‹redacted›".to_string() } else { a.clone() };
            redact_next = a == "-p" || a == "-P";
            out
        }))
        .collect();
    eprintln!("$ {}", shown.join(" "));
    let status = cmd.status().unwrap_or_else(|e| panic!("failed to run {:?}: {e}", cmd));
    if !status.success() {
        panic!("command failed: {:?}", cmd);
    }
}

fn main() {
    let task = std::env::args().nth(1).unwrap_or_else(|| "bundle".into());
    match task.as_str() {
        "bundle" => {
            bundle();
        }
        "install" => {
            let app = bundle();
            install(&app);
        }
        "run" => {
            let app = bundle();
            run(Command::new("open").arg(&app));
        }
        "dmg" => {
            let app = bundle();
            dmg(&app);
        }
        "setup-signing" => setup_signing(),
        "icon" => {
            let dist = root().join("dist");
            fs::create_dir_all(&dist).unwrap();
            make_icns(&dist);
        }
        other => {
            eprintln!("unknown task `{other}`. Use: bundle | install | run | dmg | setup-signing | icon");
            std::process::exit(2);
        }
    }
}

fn bundle() -> PathBuf {
    let root = root();
    // Universal binary (Apple silicon + Intel) when both Rust targets exist.
    let installed = {
        let (_, out) = output(Command::new("rustup").args(["target", "list", "--installed"]));
        out
    };
    let targets: Vec<&str> = ["aarch64-apple-darwin", "x86_64-apple-darwin"]
        .into_iter()
        .filter(|t| installed.contains(t))
        .collect();
    let mut binaries = Vec::new();
    if targets.is_empty() {
        run(Command::new(env!("CARGO"))
            .current_dir(&root)
            .args(["build", "--release", "--package", "snipping"]));
        binaries.push(root.join("target/release").join(EXECUTABLE));
    } else {
        for t in &targets {
            run(Command::new(env!("CARGO"))
                .current_dir(&root)
                .env("MACOSX_DEPLOYMENT_TARGET", "15.0")
                .args(["build", "--release", "--package", "snipping", "--target", t]));
            binaries.push(root.join("target").join(t).join("release").join(EXECUTABLE));
        }
    }

    let dist = root.join("dist");
    let app = dist.join(format!("{APP_NAME}.app"));
    let _ = fs::remove_dir_all(&app);
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos).unwrap();
    fs::create_dir_all(&resources).unwrap();

    if binaries.len() > 1 {
        run(Command::new("lipo").arg("-create").args(&binaries).arg("-output").arg(macos.join(EXECUTABLE)));
    } else {
        fs::copy(&binaries[0], macos.join(EXECUTABLE)).expect("copy binary");
    }
    fs::write(contents.join("Info.plist"), info_plist(&version())).unwrap();
    fs::write(contents.join("PkgInfo"), "APPL????").unwrap();
    let icns = make_icns(&dist);
    fs::copy(&icns, resources.join("AppIcon.icns")).unwrap();

    // Sign with the local certificate when it exists (Screen Recording
    // permission then survives rebuilds); otherwise ad-hoc.
    match signing_identity() {
        Some((keychain, hash)) => {
            eprintln!("Signing with local certificate {hash}");
            // codesign only searches keychains on the search list: add ours for
            // the duration of the signature, then restore the user's list.
            let _guard = SearchListGuard::add(&keychain);
            run(Command::new("codesign")
                .args(["--force", "--deep", "--keychain"])
                .arg(&keychain)
                .args(["--sign", &hash, "--identifier", BUNDLE_ID])
                .arg(&app));
        }
        None => {
            eprintln!("Signing ad-hoc (run `cargo xtask setup-signing` to keep permissions across rebuilds)");
            run(Command::new("codesign").args([
                "--force",
                "--deep",
                "--sign",
                "-",
                "--identifier",
                BUNDLE_ID,
                app.to_str().unwrap(),
            ]));
        }
    }
    run(Command::new("codesign").args(["--verify", "--verbose=1", app.to_str().unwrap()]));
    eprintln!("\n✅ Built {}", app.display());
    app
}

fn install(app: &Path) {
    let targets = [PathBuf::from("/Applications"), PathBuf::from(std::env::var("HOME").unwrap()).join("Applications")];
    for dir in targets {
        let _ = fs::create_dir_all(&dir);
        let dest = dir.join(format!("{APP_NAME}.app"));
        // Quit a running copy first so the bundle can be replaced.
        let _ = Command::new("osascript").args(["-e", &format!("quit app \"{APP_NAME}\"")]).status();
        let _ = fs::remove_dir_all(&dest);
        let ok = Command::new("ditto").arg(app).arg(&dest).status().map(|s| s.success()).unwrap_or(false);
        if ok {
            eprintln!("✅ Installed {}", dest.display());
            return;
        }
    }
    panic!("could not install into /Applications or ~/Applications");
}

// ---------------------------------------------------------------------------
// Local code signing (stable identity => macOS privacy permissions persist)
// ---------------------------------------------------------------------------

const CERT_NAME: &str = "Snipping Local Code Signing";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn signing_dir() -> PathBuf {
    home().join("Library/Application Support/Snipping/signing")
}

fn keychain_path() -> PathBuf {
    home().join("Library/Keychains/snipping-signing.keychain-db")
}

fn output(cmd: &mut Command) -> (bool, String) {
    match cmd.output() {
        Ok(o) => (
            o.status.success(),
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        ),
        Err(e) => (false, e.to_string()),
    }
}

/// (keychain, SHA-1) of a usable local signing identity, unlocking its keychain.
fn signing_identity() -> Option<(PathBuf, String)> {
    let dir = signing_dir();
    let hash = fs::read_to_string(dir.join("identity")).ok()?.trim().to_string();
    let password = fs::read_to_string(dir.join("keychain-password")).ok()?.trim().to_string();
    let kc = keychain_path();
    if hash.is_empty() || !kc.exists() {
        return None;
    }
    let (ok, _) = output(Command::new("security").args(["unlock-keychain", "-p", &password]).arg(&kc));
    if !ok {
        return None;
    }
    let (_, listing) = output(Command::new("security").args(["find-identity", "-v", "-p", "codesigning"]).arg(&kc));
    if listing.contains(&hash) {
        Some((kc, hash))
    } else {
        eprintln!("Local signing certificate is not valid/trusted any more; falling back to ad-hoc.");
        None
    }
}

fn user_search_list() -> Vec<String> {
    let (_, out) = output(Command::new("security").args(["list-keychains", "-d", "user"]));
    out.lines()
        .map(|l| l.trim().trim_matches('"').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn set_user_search_list(list: &[String]) {
    // One argument per keychain — never join paths into a single string.
    let _ = Command::new("security").args(["list-keychains", "-d", "user", "-s"]).args(list).status();
}

/// Temporarily appends a keychain to the user's search list; restores the
/// exact original list on drop (including on panic).
struct SearchListGuard {
    original: Vec<String>,
}

impl SearchListGuard {
    fn add(keychain: &Path) -> SearchListGuard {
        let original = user_search_list();
        let mut with_ours = original.clone();
        let ours = keychain.to_string_lossy().into_owned();
        if !with_ours.contains(&ours) {
            with_ours.push(ours);
        }
        set_user_search_list(&with_ours);
        SearchListGuard { original }
    }
}

impl Drop for SearchListGuard {
    fn drop(&mut self) {
        set_user_search_list(&self.original);
        if user_search_list() != self.original {
            eprintln!("⚠️  Could not restore your keychain search list. Expected: {:?}", self.original);
        }
    }
}

fn setup_signing() {
    if let Some((_, hash)) = signing_identity() {
        eprintln!("✅ Local signing is already set up ({hash}).");
        return;
    }
    let dir = signing_dir();
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let kc = keychain_path();
    let _ = Command::new("security").arg("delete-keychain").arg(&kc).status();

    // Random password for the dedicated keychain (owner-only file).
    let mut bytes = [0u8; 24];
    use std::io::Read;
    fs::File::open("/dev/urandom").unwrap().read_exact(&mut bytes).unwrap();
    let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let pw_path = dir.join("keychain-password");
    fs::write(&pw_path, &password).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&pw_path, fs::Permissions::from_mode(0o600)).unwrap();

    // Self-signed certificate valid only for code signing.
    let cnf = dir.join("cert.cnf");
    fs::write(
        &cnf,
        format!(
            "[req]\ndistinguished_name = dn\nx509_extensions = ext\nprompt = no\n[dn]\nCN = {CERT_NAME}\n\
             [ext]\nbasicConstraints = critical,CA:false\nkeyUsage = critical,digitalSignature\n\
             extendedKeyUsage = critical,codeSigning\n"
        ),
    )
    .unwrap();
    let key = dir.join("key.pem");
    let cert = dir.join("cert.pem");
    let p12 = dir.join("identity.p12");
    // /usr/bin/openssl (LibreSSL) writes PKCS#12 that `security import` accepts.
    run(Command::new("/usr/bin/openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .arg("-config")
        .arg(&cnf));
    run(Command::new("/usr/bin/openssl")
        .args(["pkcs12", "-export", "-name", CERT_NAME, "-passout"])
        .arg(format!("pass:{password}"))
        .arg("-inkey")
        .arg(&key)
        .arg("-in")
        .arg(&cert)
        .arg("-out")
        .arg(&p12));

    // Dedicated keychain (never auto-locks) holding only this identity.
    run(Command::new("security").args(["create-keychain", "-p", &password]).arg(&kc));
    run(Command::new("security").arg("set-keychain-settings").arg(&kc));
    run(Command::new("security").args(["unlock-keychain", "-p", &password]).arg(&kc));
    run(Command::new("security")
        .args(["import"])
        .arg(&p12)
        .arg("-k")
        .arg(&kc)
        .args(["-P", &password, "-T", "/usr/bin/codesign"]));
    let (ok, out) = output(
        Command::new("security")
            .args(["set-key-partition-list", "-S", "apple-tool:,apple:,codesign:", "-s", "-k", &password])
            .arg(&kc),
    );
    if !ok {
        eprintln!("warning: set-key-partition-list failed: {out}");
    }
    let _ = fs::remove_file(&key);
    let _ = fs::remove_file(&p12);

    // Trust it for code signing (macOS asks for your password once).
    eprintln!("\n🔐 macOS will now ask for your password to trust the \"{CERT_NAME}\" certificate for code signing…");
    let status = Command::new("security")
        .args(["add-trusted-cert", "-r", "trustRoot", "-p", "codeSign", "-k"])
        .arg(&kc)
        .arg(&cert)
        .status()
        .expect("security add-trusted-cert");
    if !status.success() {
        let _ = Command::new("security").arg("delete-keychain").arg(&kc).status();
        let _ = fs::remove_dir_all(&dir);
        panic!("certificate was not trusted (password prompt cancelled?) — nothing was changed");
    }

    let fingerprint = {
        let (_, out) = output(Command::new("/usr/bin/openssl").args(["x509", "-noout", "-fingerprint", "-sha1", "-in"]).arg(&cert));
        out.trim().rsplit('=').next().unwrap_or("").replace(':', "")
    };
    fs::write(dir.join("identity"), &fingerprint).unwrap();
    match signing_identity() {
        Some((_, hash)) => eprintln!("\n✅ Local signing ready ({hash}). Rebuild with `cargo xtask install`."),
        None => panic!("signing identity was created but isn't usable"),
    }
}

/// A classic drag-to-install disk image: Snipping.app + an Applications link.
fn dmg(app: &Path) {
    let dist = root().join("dist");
    let staging = dist.join("dmg-staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).unwrap();
    run(Command::new("ditto").arg(app).arg(staging.join(format!("{APP_NAME}.app"))));
    std::os::unix::fs::symlink("/Applications", staging.join("Applications")).unwrap();
    let out = dist.join(format!("{APP_NAME}.dmg"));
    let _ = fs::remove_file(&out);
    run(Command::new("hdiutil")
        .args(["create", "-volname", APP_NAME, "-fs", "HFS+", "-format", "UDZO", "-ov", "-srcfolder"])
        .arg(&staging)
        .arg(&out));
    let _ = fs::remove_dir_all(&staging);
    eprintln!("✅ Built {}", out.display());
}

fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleName</key><string>{APP_NAME}</string>
    <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
    <key>CFBundleExecutable</key><string>{EXECUTABLE}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleSignature</key><string>????</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>NSSupportsAutomaticTermination</key><false/>
    <key>NSSupportsSuddenTermination</key><false/>
    <key>NSHumanReadableCopyright</key><string>Built by nomadslayer · github.com/nomadslayer</string>
    <key>NSMicrophoneUsageDescription</key><string>Snipping records your microphone only when you turn on microphone audio for a screen recording.</string>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>Image</string>
            <key>CFBundleTypeRole</key><string>Editor</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>public.png</string>
                <string>public.jpeg</string>
                <string>com.compuserve.gif</string>
                <string>public.heic</string>
                <string>public.tiff</string>
                <string>com.microsoft.bmp</string>
                <string>public.image</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
"#
    )
}

// ---------------------------------------------------------------------------
// Icon (vector, rendered per size for crispness)
// ---------------------------------------------------------------------------

fn make_icns(dist: &Path) -> PathBuf {
    let iconset = dist.join("AppIcon.iconset");
    let _ = fs::remove_dir_all(&iconset);
    fs::create_dir_all(&iconset).unwrap();
    for (name, px) in [
        ("icon_16x16.png", 16),
        ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32),
        ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128),
        ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256),
        ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512),
        ("icon_512x512@2x.png", 1024),
    ] {
        render_icon(px).save_png(iconset.join(name)).unwrap();
    }
    render_icon(1024).save_png(dist.join("AppIcon-1024.png")).unwrap();
    let icns = dist.join("AppIcon.icns");
    run(Command::new("iconutil").args(["-c", "icns"]).arg(&iconset).arg("-o").arg(&icns));
    let _ = fs::remove_dir_all(&iconset);
    icns
}

fn color(hex: u32, a: u8) -> Color {
    Color::from_rgba8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, a)
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> SkPath {
    let k = 0.5523; // circle approximation
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + r * k, y, x + w, y + r - r * k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + r * k, x + w - r + r * k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - r * k, y + h, x, y + h - r + r * k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - r * k, x + r - r * k, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn circle(cx: f32, cy: f32, r: f32) -> SkPath {
    PathBuilder::from_circle(cx, cy, r).unwrap()
}

fn render_icon(size: u32) -> Pixmap {
    let mut pm = Pixmap::new(size, size).unwrap();
    let s = size as f32 / 1024.0;
    let base = Transform::from_scale(s, s);

    // Squircle-ish tile on the macOS icon grid (824pt content, 100pt margin).
    let tile = rounded_rect(100.0, 100.0, 824.0, 824.0, 186.0);
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.shader = LinearGradient::new(
        Point::from_xy(512.0, 100.0),
        Point::from_xy(512.0, 924.0),
        vec![GradientStop::new(0.0, color(0x4F8BFF, 255)), GradientStop::new(1.0, color(0x3446D6, 255))],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    // Soft drop shadow.
    let mut shadow = Paint::default();
    shadow.anti_alias = true;
    shadow.set_color(color(0x000000, 50));
    pm.fill_path(&tile, &shadow, FillRule::Winding, base.pre_translate(0.0, 10.0), None);
    pm.fill_path(&tile, &paint, FillRule::Winding, base, None);

    // Top highlight.
    let mut hl = Paint::default();
    hl.anti_alias = true;
    hl.shader = LinearGradient::new(
        Point::from_xy(512.0, 100.0),
        Point::from_xy(512.0, 520.0),
        vec![GradientStop::new(0.0, color(0xFFFFFF, 46)), GradientStop::new(1.0, color(0xFFFFFF, 0))],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    pm.fill_path(&tile, &hl, FillRule::Winding, base, None);

    // Dashed selection marquee.
    let marquee = rounded_rect(236.0, 250.0, 520.0, 420.0, 26.0);
    let mut white = Paint::default();
    white.anti_alias = true;
    white.set_color(color(0xFFFFFF, 255));
    let mut stroke = Stroke::default();
    stroke.width = 30.0;
    stroke.line_cap = LineCap::Round;
    stroke.dash = StrokeDash::new(vec![64.0, 44.0], 0.0);
    pm.stroke_path(&marquee, &white, &stroke, base, None);

    // Translucent "selected area" fill.
    let mut fill = Paint::default();
    fill.anti_alias = true;
    fill.set_color(color(0xFFFFFF, 40));
    pm.fill_path(&rounded_rect(258.0, 272.0, 476.0, 376.0, 14.0), &fill, FillRule::Winding, base, None);

    // Scissors, cutting in at the bottom-right corner of the marquee.
    let sc = base.pre_translate(668.0, 668.0).pre_rotate(-45.0).pre_scale(0.66, 0.66);
    let mut blade = Paint::default();
    blade.anti_alias = true;
    blade.set_color(color(0xFFFFFF, 255));
    // Blades pass through the pivot (0,0): tip = -k * handle.
    let blade_path = |hx: f32, hy: f32| -> SkPath {
        let len = (hx * hx + hy * hy).sqrt();
        let (dx, dy) = (hx / len, hy / len);
        let (px, py) = (-dy, dx);
        let tip = (-dx * 360.0, -dy * 360.0);
        let mut pb = PathBuilder::new();
        pb.move_to(tip.0, tip.1);
        pb.quad_to(px * 40.0 - dx * 150.0, py * 40.0 - dy * 150.0, px * 30.0, py * 30.0);
        pb.line_to(hx + px * 18.0, hy + py * 18.0);
        pb.line_to(hx - px * 18.0, hy - py * 18.0);
        pb.line_to(-px * 16.0, -py * 16.0);
        pb.close();
        pb.finish().unwrap()
    };
    let handles = [(56.0f32, 150.0f32), (-56.0, 150.0)];
    for (hx, hy) in handles {
        pm.fill_path(&blade_path(hx, hy), &blade, FillRule::Winding, sc, None);
    }
    let mut ring = Stroke::default();
    ring.width = 34.0;
    for (hx, hy) in handles {
        let len = (hx * hx + hy * hy).sqrt();
        let k = 1.0 + 72.0 / len;
        pm.stroke_path(&circle(hx * k, hy * k, 68.0), &blade, &ring, sc, None);
    }
    // Pivot screw.
    let mut screw = Paint::default();
    screw.anti_alias = true;
    screw.set_color(color(0x3A55E0, 255));
    pm.fill_path(&circle(0.0, 0.0, 16.0), &screw, FillRule::Winding, sc, None);
    pm
}
