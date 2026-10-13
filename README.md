# corvo-lan

*Formerly lan-chat.*

Serverless chat for your local network. Run it on two machines on the same
Wi-Fi or LAN and they find each other. There's no server, no account and no
setup.

- People find each other automatically (UDP multicast), then talk over direct TCP connections.
- Everyone gets a unique name: you choose the start and a word is added (`--nick bob` → `@bobRoger`).
- Private rooms are protected by a key and end-to-end encrypted. Only people with the room's name and key can read it.
- Direct messages go peer to peer. They are **not encrypted yet**; that comes in v0.4.
- Chats are never written to disk. Quitting forgets everything.

## Install

On macOS or Linux (x86_64 or ARM):

```bash
curl -fsSL https://raw.githubusercontent.com/Abinsanth/corvo-lan/release/install.sh | sh
```

This downloads the latest release, checks its SHA-256, and installs it to
`~/.local/bin` (set `CORVO_LAN_INSTALL_DIR` to change that). It never uses sudo.
Run the same command again to update.

Or build from source with a Rust toolchain ([rustup.rs](https://rustup.rs)):

```bash
git clone https://github.com/Abinsanth/corvo-lan.git
cd corvo-lan
cargo install --path .
```

## Use

```bash
corvo-lan                   # your name starts with $USER
corvo-lan --nick bob        # or choose the start yourself
```

After about two seconds you get a name, and you're connected to everyone else running it.

```
/create cat 123            # creates e.g. #catPieR with key 123-k7m9xq
/chat #catPieR 123-k7m9xq  # someone else joins with the name and key
/chat @bobRoger            # private chat
/msg @bobRoger hi          # one-off private message
/who                       # who's connected
/update [yes]              # check for and install a newer version
/help                      # all commands
```

Plain text goes to the conversation shown in the prompt.

### Updating

Type `/update` to see newer versions, then `/update yes` to install the
newest one; restart corvo-lan to use it. At startup corvo-lan makes one request
to GitHub to see whether a newer version exists. That request sends only your
IP address and the app version; turn it off with `--no-update-check` or
`CORVO_LAN_NO_UPDATE_CHECK=1`. Nothing else ever leaves your network.

### If peers don't find each other

Discovery uses multicast on `239.255.42.99:4299`. Some networks (guest Wi-Fi,
corporate networks, some routers) block it. In that case, connect directly:

```bash
corvo-lan --port 5000                      # on one machine
corvo-lan --peer 192.168.1.20:5000         # on the other
```

Your firewall must also allow incoming TCP on the port you use. `--debug`
writes a log to `./corvo-lan.log`.

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

CI runs all three on every pull request.

## Releasing

Work happens on `main`. To ship, bump `version` in `Cargo.toml`, then merge
`main` into the `release` branch. That builds macOS and Linux binaries,
tags `v<version>`, and publishes them as a GitHub Release, which is what the
install script downloads. A version that's already released is refused, so
bump it first.
