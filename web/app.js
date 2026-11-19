const status = document.getElementById("status");
const canvas = document.getElementById("jitserver");

const platform = navigator.userAgentData?.platform;
const userAgent = navigator.userAgent;
const ios = /iPhone|iPad|iPod/i.test(userAgent)
  || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const supportedOS = !ios && (platform
  ? ["macOS", "Linux", "Android"].includes(platform)
  : !/Windows|CrOS/i.test(userAgent) && /Macintosh|Linux|Android/i.test(userAgent));
const hasWebUSB = typeof navigator.usb?.requestDevice === "function";
const compatibilityIssues = [];
if (!hasWebUSB) compatibilityIssues.push("WebUSB is unavailable in this browser.");
if (!supportedOS) compatibilityIssues.push("Your operating system is outside the supported list.");
if (compatibilityIssues.length) {
  const warning = document.getElementById("compatibility-warning");
  const message = document.getElementById("compatibility-message");
  warning.querySelector("button").addEventListener("click", () => {
    warning.hidden = true;
  });
  warning.hidden = false;
  message.textContent = `JITserver Web is unlikely to work here. ${compatibilityIssues.join(" ")} Use a Chromium-based browser with WebUSB on macOS, Linux, or Android.`;
}

try {
  const { default: init, start } = await import("./pkg/jitserver_web.js");
  await init();
  canvas.hidden = false;
  await start(canvas);
  status.remove();
} catch (error) {
  canvas.hidden = true;
  status.textContent = `JITserver failed to load: ${error?.message ?? error}`;
}
