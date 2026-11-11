#!/usr/bin/env node
// Stages and archives the desktop app (apps/desktop) for one target. Called by release.yml, runnable locally.
//   node scripts/package-desktop.mjs --target aarch64-apple-darwin --version 0.1.0 --bin target/release/synthesis-desktop --out dist
// Output: <out>/synthesis-desktop-<version>-<target>.{zip|tar.gz}
//   macOS   .zip of Synthesis.app (Info.plist, synthesis.icns; ad-hoc signed, NOT notarized)
//   Windows .zip with Synthesis.exe (icon embedded by apps/desktop/build.rs)
//   Linux   .tar.gz with synthesis-desktop, synthesis.desktop, hicolor icons, README.txt
// Needs: ditto + codesign (macOS), 7z (Windows), tar (Linux), all preinstalled on the runners.
import { copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = join(fileURLToPath(import.meta.url), "..", "..");
const ID = "dev.synthesis.desktop"; // apps/desktop/Cargo.toml [package.metadata.bundle]
const SIZES = [16, 32, 64, 128, 256, 512];

export function infoPlist(version) {
  const build = version.match(/^\d+(\.\d+){0,2}/)?.[0];
  if (!build) throw new Error(`bad version: ${version}`);
  const kv = (k, v) => `\t<key>${k}</key>\n\t${v}`;
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
${[
  kv("CFBundleName", "<string>Synthesis</string>"),
  kv("CFBundleDisplayName", "<string>Synthesis</string>"),
  kv("CFBundleIdentifier", `<string>${ID}</string>`),
  kv("CFBundleExecutable", "<string>Synthesis</string>"),
  kv("CFBundleIconFile", "<string>synthesis</string>"),
  kv("CFBundlePackageType", "<string>APPL</string>"),
  kv("CFBundleShortVersionString", `<string>${version}</string>`),
  kv("CFBundleVersion", `<string>${build}</string>`),
  kv("LSApplicationCategoryType", "<string>public.app-category.developer-tools</string>"),
  kv("NSHighResolutionCapable", "<true/>"),
  kv("NSPrincipalClass", "<string>NSApplication</string>"),
].join("\n")}
</dict>
</plist>
`;
}

export function desktopEntry() {
  return `[Desktop Entry]
Type=Application
Name=Synthesis
Comment=Autonomous coding agent
Exec=synthesis-desktop
Icon=synthesis
Terminal=false
Categories=Development;
StartupWMClass=synthesis-desktop
`;
}

const linuxReadme = (v) => `Synthesis desktop ${v} (Linux)

Run in place:   ./synthesis-desktop [--cwd DIR]
Install (user): install -Dm755 synthesis-desktop ~/.local/bin/synthesis-desktop
                install -Dm644 synthesis.desktop ~/.local/share/applications/synthesis.desktop
                cp -r icons/hicolor ~/.local/share/icons/   (then: gtk-update-icon-cache -t ~/.local/share/icons/hicolor)
Start from a launcher: the window works on the folder the app is started in (your home folder by default).

Runtime libraries (Debian/Ubuntu package names; the binary loads them at start-up):
  libxcb1 libx11-xcb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-cursor0 libwayland-egl1
  libfontconfig1 libfreetype6 libvulkan1 (plus a Vulkan driver: mesa-vulkan-drivers, or your GPU vendor's)
A display server (X11 or Wayland) is required. glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+).

Not signed. Verify the download with SHA256SUMS and "gh attestation verify" (docs/INSTALL.md).
`;

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { stdio: "inherit", ...opts });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed (${r.status ?? r.error})`);
}

function main() {
  const a = {};
  for (let i = 2; i < process.argv.length; i += 2) a[process.argv[i].replace(/^--/, "")] = process.argv[i + 1];
  const { target, version, bin, out } = a;
  if (!target || !version || !bin || !out) throw new Error("usage: --target T --version V --bin PATH --out DIR");
  if (!/^[0-9A-Za-z.+-]+$/.test(version)) throw new Error(`bad version: ${version}`);
  if (!existsSync(bin)) throw new Error(`binary not found: ${bin}`);

  const name = `synthesis-desktop-${version}-${target}`;
  const stage = join(out, name);
  rmSync(stage, { recursive: true, force: true });
  mkdirSync(stage, { recursive: true });
  const icons = join(root, "design", "01-brand", "app-icons");
  const assets = join(root, "apps", "desktop", "assets", "icons");
  const legal = () => ["LICENSE", "NOTICE"].forEach((f) => copyFileSync(join(root, f), join(stage, f)));

  if (target.includes("apple-darwin")) {
    const app = join(stage, "Synthesis.app", "Contents");
    mkdirSync(join(app, "MacOS"), { recursive: true });
    mkdirSync(join(app, "Resources"), { recursive: true });
    copyFileSync(bin, join(app, "MacOS", "Synthesis"));
    cpSync(join(assets, "synthesis.icns"), join(app, "Resources", "synthesis.icns"));
    writeFileSync(join(app, "Info.plist"), infoPlist(version));
    legal();
    // Ad-hoc seal (no identity, no secrets): keeps the bundle consistent; Gatekeeper still treats a download as unnotarized.
    run("codesign", ["--force", "--sign", "-", join(stage, "Synthesis.app")]);
    run("ditto", ["-c", "-k", "--keepParent", "--norsrc", "--noextattr", join(stage, "Synthesis.app"), join(out, `${name}.zip`)]);
  } else if (target.includes("windows")) {
    copyFileSync(bin, join(stage, "Synthesis.exe"));
    legal();
    run("7z", ["a", "-tzip", `${name}.zip`, name], { cwd: out, stdio: ["ignore", "ignore", "inherit"] });
  } else if (target.includes("linux")) {
    copyFileSync(bin, join(stage, "synthesis-desktop"));
    writeFileSync(join(stage, "synthesis.desktop"), desktopEntry());
    writeFileSync(join(stage, "README.txt"), linuxReadme(version));
    for (const s of SIZES) {
      const d = join(stage, "icons", "hicolor", `${s}x${s}`, "apps");
      mkdirSync(d, { recursive: true });
      copyFileSync(join(icons, `synthesis-app-icon-${s}.png`), join(d, "synthesis.png"));
    }
    legal();
    run("tar", ["-czf", `${name}.tar.gz`, name], { cwd: out });
  } else {
    throw new Error(`unsupported target: ${target}`);
  }
  console.log(`packaged ${name} in ${out}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try { main(); } catch (e) { console.error(`package-desktop: ${e.message}`); process.exit(1); }
}
