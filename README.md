# <img height="35" src="/Linux.png" alt="Logo"> JITserver

A cross-platform GUI application that enables JIT for iOS devices.

## Features

- **Device Management**: Automatically discover and connect to iOS devices via USB
- **JIT Enabling**: Self-explanatory.
- **App Integration**: Support to enable JIT for popular apps (even on iOS 26+) including:
  - Amethyst
  - MeloNX
  - MeloCafé
  - Manic EMU
  - Geode
  - UTM
  - DolphiniOS
  - Flycast-iOS 26 fork
- **Developer Mode**: Monitor developer mode status
- **Run Scripts**: Run Scripts required for JIT on TXM devices on iOS 26+
  - Built-in scripts are bundled into the app for release builds
  - Imported custom scripts are stored in the user's app data directory
- **Developer Disk Image Mounting**: Automatically mount required developer images for iOS 17+

## Prerequisites

- **macOS/Linux/Windows**: Cross-platform support, must have usbmuxd installed
- **iOS/iPadOS Device**: Must have a passcode set and be connected via USB
- **Rust**: Required for building from source

## Building from Source

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

## Usage

### Getting Started

1. **Connect your iOS device** via USB to your computer, or over Local Network if on the same Wi-Fi and previously connected
2. **Launch the application** - it will automatically scan for connected devices
3. **Select your device** from the dropdown menu if not already selected


## Install Guide

### Prerequisites for JIT enabling

Before enabling JIT, make sure you have:

1. **Sideloaded an app** (can be done with [SideStore](https://sidestore.io/) or a certificate + signer)

2. **Enabled Developer Mode** on your iOS/iPadOS device (found in Settings → Privacy & Security after sideloading an app, required for iOS 17+)

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

### Enabling JIT Instructions

1. **Open JITserver** and select your device from the dropdown menu
2. **Connect your device** to your computer via USB cable if it doesn't already appear
   - If prompted, select `Trust` and enter your passcode

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

This project is licensed under the AGPL-3.0 License.

## Acknowledgments

- Built with the [idevice](https://crates.io/crates/idevice) library for iOS device communication
- GUI powered by [egui](https://github.com/emilk/egui)
- Heavily based on [StikDebug](https://github.com/StephenDev0/StikDebug)'s JIT implementation
