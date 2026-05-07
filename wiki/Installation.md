# Installation

<div align="center">
  <a href="https://flathub.org/apps/dev.nicx.mimick">
    <img width="240" alt="Get it on Flathub" src="https://flathub.org/api/badge?locale=en">
  </a>
</div>

The recommended install method is via Flathub.

## Prerequisites

- **Flatpak** must be installed on your system.
- The **[Flathub](https://flathub.org/setup)** remote must be configured. Follow the setup guide for your distribution at [flathub.org/setup](https://flathub.org/setup).

## Install Mimick

### Command Line Install

```bash
flatpak install flathub dev.nicx.mimick
```

*(Alternatively, you can install it graphically through your system's software center if it has Flathub integration, like GNOME Software or KDE Discover).*

## Running

Run the app with:

```bash
flatpak run dev.nicx.mimick
```

Open the settings window directly with:

```bash
flatpak run dev.nicx.mimick --settings
```

## Local Development Build

For a native development run:

```bash
cargo run
```

Open settings immediately:

```bash
cargo run -- --settings
```

For a local Flatpak build that uses the current checkout instead of the GitHub source tag:

```bash
flatpak-builder --user --install --force-clean build-dir dev.nicx.mimick.local.yml
```

## What Gets Installed

- Application ID: `dev.nicx.mimick`
- Binary: `mimick`
- Config file: `~/.config/mimick/config.json` (native) or `~/.var/app/dev.nicx.mimick/config/mimick/config.json` (Flatpak)
- Cache directory: `~/.cache/mimick/` (native) or `~/.var/app/dev.nicx.mimick/cache/mimick/` (Flatpak)
- Keyring: Managed by `oo7` -- encrypted file inside the sandbox (Flatpak) or D-Bus Secret Service (native)
