//! Startup checks, and the guidance to fix whatever is missing.
//!
//! Everything `stereowire` needs is something the user has to set up once: Tailscale
//! on both machines, and Screen Recording permission on the sending one. This
//! finds the gaps and says exactly how to close them, rather than failing with
//! a socket error.

use std::path::PathBuf;
use std::process::Command;

/// Places the Tailscale CLI lands, depending on how it was installed.
#[cfg(target_os = "macos")]
const CLI_PATHS: [&str; 3] = [
    // The Mac App Store and standalone app bundle the CLI inside the app.
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
];

#[cfg(windows)]
const CLI_PATHS: [&str; 1] = [r"C:\Program Files\Tailscale\tailscale.exe"];

pub struct Finding {
    pub ok: bool,
    pub what: String,
    /// Shown only when the check failed.
    pub fix: Option<String>,
}

impl Finding {
    fn ok(what: impl Into<String>) -> Self {
        Finding {
            ok: true,
            what: what.into(),
            fix: None,
        }
    }

    fn bad(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Finding {
            ok: false,
            what: what.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Locates the Tailscale CLI, if it is installed at all.
pub fn tailscale_cli() -> Option<PathBuf> {
    #[cfg(windows)]
    let found = Command::new("where").arg("tailscale").output();
    #[cfg(not(windows))]
    let found = Command::new("/usr/bin/which").arg("tailscale").output();

    if let Ok(output) = found {
        if output.status.success() {
            // `where` can print one match per line; `which` prints exactly one.
            let path = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    CLI_PATHS.iter().map(PathBuf::from).find(|p| p.exists())
}

fn run(cli: &PathBuf, args: &[&str]) -> Option<(bool, String)> {
    let output = Command::new(cli).args(args).output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some((output.status.success(), text))
}

#[cfg(target_os = "macos")]
const INSTALL: &str = "install it with `brew install --cask tailscale`, or from \
https://tailscale.com/download/macos, then run `tailscale up`";

#[cfg(windows)]
const INSTALL: &str = "install it from https://tailscale.com/download/windows, then sign in \
from the tray icon";

/// Runs every check that applies. `peer` is the address `send` will dial.
pub fn check(peer: Option<&str>, needs_screen_recording: bool) -> Vec<Finding> {
    // Screen Recording is a macOS concept; on other platforms the flag has
    // nothing to check against.
    #[cfg(not(target_os = "macos"))]
    let _ = needs_screen_recording;

    let mut findings = Vec::new();

    let Some(cli) = tailscale_cli() else {
        findings.push(Finding::bad("Tailscale is not installed", INSTALL));
        // Every later check needs the CLI, so stop here.
        #[cfg(target_os = "macos")]
        if needs_screen_recording {
            findings.push(screen_recording());
        }
        return findings;
    };
    findings.push(Finding::ok("Tailscale is installed"));

    match run(&cli, &["status"]) {
        Some((true, _)) => findings.push(Finding::ok("Tailscale is running")),
        Some((false, text)) => {
            let hint = if text.contains("Logged out") || text.contains("logged out") {
                "run `tailscale up` and sign in"
            } else if text.contains("stopped") {
                "start Tailscale from the menu bar or tray icon, or run `tailscale up`"
            } else {
                "run `tailscale status` to see why"
            };
            findings.push(Finding::bad("Tailscale is not connected", hint));
        }
        None => findings.push(Finding::bad(
            "could not run the Tailscale CLI",
            "check that Tailscale is not mid-update",
        )),
    }

    // Whether the peer is reachable, and just as importantly whether the
    // connection is direct: a relayed link adds latency the distance does not
    // explain.
    if let Some(peer) = peer {
        match run(&cli, &["ping", "--c", "1", peer]) {
            Some((true, text)) if text.contains("via DERP") => findings.push(Finding::bad(
                format!("{peer} is reachable but RELAYED, which adds latency"),
                "both machines should be on Ethernet with UPnP or NAT-PMP enabled on \
the router; run `tailscale netcheck` for details",
            )),
            Some((true, _)) => findings.push(Finding::ok(format!("{peer} is reachable, direct"))),
            Some((false, _)) | None => findings.push(Finding::bad(
                format!("{peer} is not reachable"),
                "check the name against `tailscale status`, and that the other machine is \
online and signed in to the same tailnet",
            )),
        }
    }

    #[cfg(target_os = "macos")]
    if needs_screen_recording {
        findings.push(screen_recording());
    }
    findings
}

#[cfg(target_os = "macos")]
fn screen_recording() -> Finding {
    // Preflight asks without prompting, so a missing permission can be
    // explained rather than surprising the user with a system dialog.
    if objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        Finding::ok("Screen Recording permission granted")
    } else {
        Finding::bad(
            "Screen Recording permission is missing",
            "grant it in System Settings > Privacy & Security > Screen & System Audio \
Recording, then run stereowire again",
        )
    }
}

/// Prints the findings. Returns false if anything needs attention.
pub fn report(findings: &[Finding]) -> bool {
    let failures = findings.iter().filter(|f| !f.ok).count();
    if failures == 0 {
        println!(
            "checks passed: {}",
            findings
                .iter()
                .map(|f| f.what.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return true;
    }
    println!("setup needs attention:");
    for finding in findings {
        println!("  {} {}", if finding.ok { "✓" } else { "✗" }, finding.what);
        if let Some(fix) = &finding.fix {
            println!("      {fix}");
        }
    }
    if findings
        .iter()
        .any(|f| !f.ok && f.what.contains("not installed"))
    {
        println!();
        println!("Tailscale gives both machines a stable private address and connects them");
        println!("directly, so stereowire needs no port forwarding and no public server.");
        println!("Both of you must be on the same tailnet: sign in to the same account,");
        println!("or invite the other from the admin console at https://login.tailscale.com");
    }
    false
}
