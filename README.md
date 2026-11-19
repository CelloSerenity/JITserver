# <img height="35" src="/Linux.png" alt="Logo"> JITserver

A cross-platform GUI application that enables JIT for iOS devices.

## Features

- **Device Management**: Automatically discover and connect to iOS devices via USB
- **JIT Enabling**: Exposing developer mode, mounting Developer Disk Image, attaching debugger, and running scripts (iOS 26 TXM)

## Supported apps

Desktop and web recommend bundled scripts for these apps on devices that require a script. The mappings follow [StikDebug’s script compatibility list](https://github.com/StikDebug/StikDebug/blob/main/SUPPORTED_APPS.md).

| Bundled script | Apps |
| --- | --- |
| [universal.js](scripts/universal.js) | Amethyst, Applesauce, DukeX, Henge, Husk, Manic EMU, MeloCafé, MeloNX, RPCS3, Utsuro, XeniOS |
| [legacy.js](scripts/legacy.js) | ARMSX2 iOS, DolphiniOS, Flycast for iOS, UTM |
| [Geode.js](scripts/Geode.js) | Geode |

JITserver also recommends `universal.js` for AetherPS4 and [maciOS.js](scripts/maciOS.js) for maciOS. For apps that enable JIT internally, see StikDebug’s separate [Built-in JIT list](https://github.com/StikDebug/StikDebug/blob/main/SUPPORTED_APPS.md#built-in-jit).

## Install/Build Prerequisites

- **macOS/Linux/Windows**: Must have usbmuxd installed
- **iOS/iPadOS Device**: Must have a passcode set and be connected via USB
- **Rust**: Required for building from source

## Usage

### Installation Instructions

#### macOS
1. Download [JITserver for macOS](https://github.com/CelloSerenity/JITserver/releases/latest/download/JITserver-macos-universal.dmg)
2. Open the Disk Image and drag `JITserver` to `Applications`

#### Windows
1. Install [Apple Devices](https://apps.microsoft.com/detail/9np83lwlpz9k) from the Microsoft Store or [iTunes](https://apple.com/itunes/download/win64) from Apple's website
2. Download [JITserver for Windows](https://github.com/CelloSerenity/JITserver/releases/latest/download/JITserver-windows-x86_64.exe) and save it to a memorable location

#### Linux
1. Install usbmuxd: 
   ```bash
   sudo apt install -y usbmuxd
   ```
2. Download JITserver for Linux for your machine's architecture and save it to a memorable location:
   - [x86_64](https://github.com/CelloSerenity/JITserver/releases/latest/download/JITserver-linux-x86_64.AppImage)
   - [AArch64](https://github.com/CelloSerenity/JITserver/releases/latest/download/JITserver-linux-aarch64.AppImage)
3. Make the downloaded file executable

Importing scripts opens the desktop file chooser through XDG Desktop Portal. On minimal Linux installations, install `xdg-desktop-portal` and a backend for your desktop environment.

### Prerequisites for JIT

Before enabling JIT, make sure you have:

1. **Sideloaded an app** with `get-task-allow` (can be done with [SideStore](https://sidestore.io/) or a developer [**not** enterprise] certificate + signer)
2. **Enabled Developer Mode** on your iOS/iPadOS device (found in Settings → Privacy & Security after sideloading an app, required on iOS 17+)

### App Setup

1. **Connect your iOS device** via USB to your computer, or over Local Network if on the same Wi-Fi and previously connected
2. **Launch the application** - it will automatically scan for connected devices
3. **Select your device** from the dropdown menu if not already selected

### Enabling JIT

1. **Open JITserver** and select your device from the dropdown menu
2. **Connect your device** to your computer via USB cable if it doesn't already appear
   - If prompted, select `Trust` and enter your passcode
3. **Enable JIT** for your app via launching and attaching to the app or attaching to a listed PID (you can search for the app's name). You can also attach+run the selected script.


## Building from Source

### Browser build

JITserver Web uses egui and shares the desktop app’s target lists, script picker, JIT controls, and log panel. It connects directly over WebUSB in a Chromium-based browser on macOS, Linux, or Android. [Helium](https://helium.computer/) is recommended on desktop. Connected iPhones and iPads require OS 17.4 or later. It can pair a device, list apps signed for debugging, and attach and detach the debugger when launching an app or enabling JIT for an existing PID. In the **PID** panel, select a running process or enter its PID manually. Pairing records are saved per device in this site’s browser storage and reused on reconnect. Clear the site’s data to remove them. **Connect** opens the WebUSB picker. Use **Pair** to create or replace the saved lockdown pairing and automatically load device status, apps, and PIDs. Once paired, **Export pairing file** creates and downloads a remote `pairingFile.plist` over the trusted connection.

Start with an unlocked USB device and Developer Mode enabled. The web app checks and mounts the Developer Disk Image automatically after connecting with a saved pairing, after pairing, or on Refresh when Developer Mode is enabled. Use **Mount** in Device Status to retry. When Apple needs to personalize the image, the browser sends the signing request through the configured proxy. Signing requests may include device identifiers. For devices that require a script, choose **Geode.js**, **legacy.js**, **maciOS.js**, or **universal.js**, then click **Enable JIT via Script** under Apps or PID. Other devices show **Enable JIT**. Keep the page open while it handles breakpoints; script messages appear in Logs. Devices connected over Wi-Fi still require the desktop app.

The **Build JITserver Web** action produces a `jitserver-web` static site artifact. With **Settings → Pages → Source** set to **GitHub Actions**, pushes to the default branch or `web-jit-support` that change the web build publish the site over HTTPS when the branch is allowed by the `github-pages` environment. Once the workflow exists on the default branch, you can also select an allowed branch under **Actions → Build JITserver Web → Run workflow**.


To enable Apple signing in a web build, deploy the Worker in `web/ddi-worker` with Wrangler and set the GitHub Actions repository variable `JITSERVER_TSS_PROXY_URL` to its HTTPS `/tss` URL. The Worker forwards only to Apple’s TSS endpoint. Its `ALLOWED_ORIGINS` setting must include the web app’s origin. For a local build, supply the same URL as a build environment variable and include your local preview origin in the Worker configuration.

For a local preview, serve the built `web/dist` directory over HTTP on localhost. Opening the source HTML directly cannot load the generated WebAssembly module.

### Desktop build

1. Clone the repository:
   ```bash
   git clone https://github.com/CelloSerenity/JITserver
   cd JITserver
   ```

2. Build the application:
   ```bash
   cargo build --release
   ```

3. Run the application:
   ```bash
   cargo run --release
   ```

## Dependencies

This project uses several key dependencies:

- **[idevice](https://crates.io/crates/idevice)**: Core iOS device communication library
- **[egui](https://crates.io/crates/egui)**: Immediate mode GUI framework
- **[tokio](https://crates.io/crates/tokio)**: Asynchronous runtime

For a complete list of dependencies, see [`Cargo.toml`](Cargo.toml).

## Troubleshooting

### Device Not Detected

- Ensure your iOS device is connected via USB
- Check that the device is trusted on your computer
- Try disconnecting and reconnecting the device

## Contributing

Contributions are welcome! Please feel free to submit issues, feature requests, or pull requests.

## License

This project is licensed under the AGPL-3.0 License for non-commercial use. If you wish to use this code commercially, you must obtain a written and explicit license from CelloSerenity.

## Acknowledgments

- Built with the [idevice](https://crates.io/crates/idevice) library for iOS device communication
- GUI powered by [egui](https://github.com/emilk/egui)
- Heavily based on [StikDebug](https://github.com/StephenDev0/StikDebug)'s JIT implementation
