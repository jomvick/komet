/**
 * Best-effort OS/arch detection for the hero download button.
 * Returns the PREFERRED asset suffix to auto-select, or null when unknown.
 */
export function detectPlatformAsset(): string | null {
  if (typeof navigator === "undefined") return null;

  const ua = navigator.userAgent;
  const uaData = (
    navigator as Navigator & {
      userAgentData?: { platform?: string; architecture?: string };
    }
  ).userAgentData;
  const platform = (uaData?.platform ?? navigator.platform ?? "").toLowerCase();
  const arch = (uaData?.architecture ?? "").toLowerCase();

  const isWindows =
    platform.includes("win") || /windows nt/i.test(ua);
  const isMac =
    platform.includes("mac") || /mac os x|macintosh/i.test(ua);
  const isLinux =
    platform.includes("linux") || /linux|x11/i.test(ua);

  if (isWindows) return "windows-x86_64.exe";
  if (isMac) return "macos-arm64.dmg";
  if (isLinux) {
    if (arch.includes("arm") || arch.includes("aarch64")) {
      return "linux-aarch64.tar.gz";
    }
    // Prefer distro packages when the UA hints at them.
    if (/ubuntu|debian|linux mint|pop!_os|elementary/i.test(ua)) return "_amd64.deb";
    if (/fedora|rhel|centos|rocky|alma|nobara/i.test(ua)) return "x86_64.rpm";
    return "linux-x86_64.tar.gz";
  }
  return null;
}

/** Human-readable platform name for the auto-detected asset suffix. */
export function platformLabel(asset: string): string {
  if (asset.includes("macos")) return "macOS";
  if (asset.includes("windows")) return "Windows";
  if (asset.includes(".deb")) return "Linux (.deb)";
  if (asset.includes(".rpm")) return "Linux (.rpm)";
  if (asset.includes("AppImage")) return "Linux (AppImage)";
  if (asset.includes("aarch64")) return "Linux (arm64)";
  if (asset.includes("linux")) return "Linux";
  return "your platform";
}
